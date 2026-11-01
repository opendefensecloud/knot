//! Personal API tokens — bearer auth on `/api/*` for bot/script accounts.
//!
//! The token string (`knot_<43 url-safe chars>`, 32 CSPRNG bytes) is shown
//! once at creation; only its SHA-256 hash is stored. Hashing happens in
//! the caller (`knot-server::auth`) so the raw secret never crosses into
//! storage code past `create`.

use async_trait::async_trait;
use base64::Engine;
use chrono::{DateTime, Utc};
use rand::TryRng;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum ApiTokenError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

pub type Result<T> = std::result::Result<T, ApiTokenError>;

/// The valid scope vocabulary. Everything else is rejected at creation.
pub const API_SCOPES: [&str; 4] = ["read", "create", "write", "delete"];

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiToken {
    pub id: Uuid,
    pub user_id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// A freshly minted token: the row plus the plaintext secret, which is
/// returned to the caller exactly once and never stored.
#[derive(Debug)]
pub struct ApiTokenSecret {
    pub token: ApiToken,
    pub secret: String,
}

/// At-rest identity of a token (`knot_…`): plain SHA-256 of the full
/// string. No key — the token is 256-bit random, so an unkeyed hash is
/// already unguessable, and lookup is by hash (unique index), which a
/// keyed HMAC would not improve.
pub fn api_token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

fn fresh_secret() -> String {
    let mut bytes = [0u8; 32];
    // See session_token.rs: rand 0.10's SysRng is fallible, and a failure to
    // reach the OS CSPRNG must not degrade into a guessable token.
    rand::rngs::SysRng
        .try_fill_bytes(&mut bytes)
        .expect("OS CSPRNG unavailable");
    format!(
        "knot_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

type TokenRow = (
    Uuid,
    Uuid,
    Uuid,
    String,
    Vec<String>,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);

fn token_from_row(r: TokenRow) -> ApiToken {
    ApiToken {
        id: r.0,
        user_id: r.1,
        workspace_id: r.2,
        name: r.3,
        scopes: r.4,
        created_at: r.5,
        last_used_at: r.6,
        revoked_at: r.7,
    }
}

#[async_trait]
pub trait ApiTokenStore: Send + Sync {
    /// Mint a token for `user_id` in `workspace_id`. `scopes` must be a
    /// non-empty subset of [`API_SCOPES`] (validated here — the DB CHECK only
    /// catches emptiness).
    async fn create(
        &self,
        user_id: Uuid,
        workspace_id: Uuid,
        name: &str,
        scopes: &[String],
    ) -> Result<ApiTokenSecret>;

    /// All tokens of a user, newest first, revoked included (revocation is
    /// visible; the secret never comes back).
    async fn list_for_user(&self, user_id: Uuid, workspace_id: Uuid) -> Result<Vec<ApiToken>>;

    /// Revoke a token. `false` when it belongs to another user/workspace
    /// (IDOR guard). Idempotent on already-revoked tokens (returns true).
    async fn revoke(&self, id: Uuid, user_id: Uuid, workspace_id: Uuid) -> Result<bool>;

    /// Look up a live token by its SHA-256 hash. `None` when unknown,
    /// revoked, or its holder is no longer a member of the workspace.
    async fn find_alive(&self, token_hash: &[u8]) -> Result<Option<ApiToken>>;

    /// Bump `last_used_at = now()`; ignored if the row doesn't exist.
    async fn touch(&self, id: Uuid) -> Result<()>;
}

pub struct PgApiTokenStore {
    pool: PgPool,
}

impl PgApiTokenStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ApiTokenStore for PgApiTokenStore {
    async fn create(
        &self,
        user_id: Uuid,
        workspace_id: Uuid,
        name: &str,
        scopes: &[String],
    ) -> Result<ApiTokenSecret> {
        if scopes.is_empty() || scopes.iter().any(|s| !API_SCOPES.contains(&s.as_str())) {
            // The route validates first; this is the trust-boundary backstop.
            return Err(ApiTokenError::Sqlx(sqlx::Error::Protocol(
                "invalid api-token scopes".into(),
            )));
        }
        let secret = fresh_secret();
        let hash = api_token_hash(&secret);
        let row = sqlx::query_as::<_, TokenRow>(
            "INSERT INTO api_tokens (user_id, workspace_id, name, token_hash, scopes)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING id, user_id, workspace_id, name, scopes, created_at, last_used_at, revoked_at",
        )
        .bind(user_id)
        .bind(workspace_id)
        .bind(name)
        .bind(&hash)
        .bind(scopes)
        .fetch_one(&self.pool)
        .await?;
        Ok(ApiTokenSecret {
            token: token_from_row(row),
            secret,
        })
    }

    async fn list_for_user(&self, user_id: Uuid, workspace_id: Uuid) -> Result<Vec<ApiToken>> {
        let rows = sqlx::query_as::<_, TokenRow>(
            "SELECT id, user_id, workspace_id, name, scopes, created_at, last_used_at, revoked_at
             FROM api_tokens
             WHERE user_id = $1 AND workspace_id = $2
             ORDER BY created_at DESC",
        )
        .bind(user_id)
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(token_from_row).collect())
    }

    async fn revoke(&self, id: Uuid, user_id: Uuid, workspace_id: Uuid) -> Result<bool> {
        let res = sqlx::query(
            "UPDATE api_tokens SET revoked_at = now()
             WHERE id = $1 AND user_id = $2 AND workspace_id = $3",
        )
        .bind(id)
        .bind(user_id)
        .bind(workspace_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn find_alive(&self, token_hash: &[u8]) -> Result<Option<ApiToken>> {
        let row = sqlx::query_as::<_, TokenRow>(
            "SELECT t.id, t.user_id, t.workspace_id, t.name, t.scopes,
                    t.created_at, t.last_used_at, t.revoked_at
             FROM api_tokens t
             JOIN workspace_members m
               ON m.workspace_id = t.workspace_id AND m.user_id = t.user_id
             WHERE t.token_hash = $1 AND t.revoked_at IS NULL",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(token_from_row))
    }

    async fn touch(&self, id: Uuid) -> Result<()> {
        sqlx::query("UPDATE api_tokens SET last_used_at = now() WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
