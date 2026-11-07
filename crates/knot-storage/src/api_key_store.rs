//! API-key rows — scoped bearer credentials for humans, bots and AI agents.
//!
//! Only the keyed HMAC of the secret (`key_hash`) is stored; it is written on
//! insert and used as the lookup key, but never read back out of the store.
//! [`ApiKey`] deliberately has no hash field.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

/// SQLSTATE raised by the `api_keys_sso_ttl_guard` trigger.
const SSO_TTL_ERRCODE: &str = "KB003";

/// Most live (not revoked, not expired) keys one user may hold. Minting
/// more answers [`ApiKeyStoreError::LimitReached`].
pub const MAX_LIVE_KEYS_PER_USER: i64 = 50;

/// How long a revoked or expired key stays in [`ApiKeyStore::list_for_user`].
pub const LIST_RETENTION_DAYS: i32 = 30;

/// One API-key permission. Independent bits: no scope implies another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Read,
    Create,
    Update,
    Delete,
}

impl Scope {
    /// Every scope, in the stable order used by [`Scopes::names`].
    const ALL: [Scope; 4] = [Scope::Read, Scope::Create, Scope::Update, Scope::Delete];

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Create => "create",
            Scope::Update => "update",
            Scope::Delete => "delete",
        }
    }

    pub fn parse(s: &str) -> Option<Scope> {
        Scope::ALL.into_iter().find(|scope| scope.as_str() == s)
    }

    fn bit(self) -> u8 {
        match self {
            Scope::Read => 1,
            Scope::Create => 2,
            Scope::Update => 4,
            Scope::Delete => 8,
        }
    }
}

/// A non-empty set of [`Scope`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scopes(u8);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ScopeError {
    #[error("no scopes given")]
    Empty,
    #[error("unknown scope: {0}")]
    Unknown(String),
}

impl Scopes {
    /// Parse scope names. Empty input or any unknown name is an error;
    /// duplicates are harmless.
    pub fn from_names(names: &[String]) -> Result<Self, ScopeError> {
        if names.is_empty() {
            return Err(ScopeError::Empty);
        }
        let mut bits = 0u8;
        for n in names {
            let scope = Scope::parse(n).ok_or_else(|| ScopeError::Unknown(n.clone()))?;
            bits |= scope.bit();
        }
        Ok(Scopes(bits))
    }

    pub fn contains(&self, s: Scope) -> bool {
        self.0 & s.bit() != 0
    }

    /// Names in stable order: read, create, update, delete.
    pub fn names(&self) -> Vec<String> {
        Scope::ALL
            .into_iter()
            .filter(|s| self.contains(*s))
            .map(|s| s.as_str().to_string())
            .collect()
    }
}

/// An API-key row without its hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKey {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub user_id: Uuid,
    pub name: String,
    pub scopes: Scopes,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Insert payload. `key_hash` is the keyed HMAC of the secret, never the
/// plaintext key.
pub struct NewApiKey<'a> {
    pub workspace_id: Uuid,
    pub user_id: Uuid,
    pub name: &'a str,
    pub key_hash: &'a [u8],
    pub scopes: &'a Scopes,
    pub created_by: Uuid,
    pub expires_at: Option<DateTime<Utc>>,
    /// The hashed id of the cookie session that asked for the key. When
    /// set, the insert is refused unless that session is still active for
    /// `created_by` in `workspace_id`, checked inside the insert's
    /// transaction.
    pub session_hash: Option<&'a [u8]>,
}

impl std::fmt::Debug for NewApiKey<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewApiKey")
            .field("workspace_id", &self.workspace_id)
            .field("user_id", &self.user_id)
            .field("name", &self.name)
            .field("key_hash", &"<redacted>")
            .field("scopes", &self.scopes)
            .field("created_by", &self.created_by)
            .field("expires_at", &self.expires_at)
            .field("session_hash", &self.session_hash.map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum ApiKeyStoreError {
    /// The SSO TTL trigger (KB003) refused the expiry.
    #[error("api key expiry rejected for OIDC-only user")]
    TtlRejected,
    /// The user already holds [`MAX_LIVE_KEYS_PER_USER`] live keys.
    #[error("api key limit reached")]
    LimitReached,
    /// The minting session is gone, or its user is no longer a member of
    /// the workspace.
    #[error("api key minting session ended")]
    SessionEnded,
    /// The key is for someone else, and that someone is not a bot member of
    /// the workspace or the creator is no longer its owner.
    #[error("api key mint not permitted")]
    NotPermitted,
    /// A stored row carries a scope this build does not know.
    #[error("stored scopes invalid: {0}")]
    BadScopes(#[from] ScopeError),
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

#[async_trait]
pub trait ApiKeyStore: Send + Sync + 'static {
    /// Insert a key, in one transaction with the facts that authorize it:
    /// the user (and creator) rows are locked, so a concurrent password
    /// change waits for the key and then revokes it, or runs first and ends
    /// `session_hash`; the membership rows are share-locked, so a concurrent
    /// removal or demotion either runs first and the mint is refused, or
    /// waits and then its trigger revokes the key. Refused with
    /// [`ApiKeyStoreError::SessionEnded`], [`ApiKeyStoreError::NotPermitted`]
    /// or [`ApiKeyStoreError::LimitReached`] (the user holds
    /// [`MAX_LIVE_KEYS_PER_USER`] live keys); KB003 from the SSO trigger
    /// maps to [`ApiKeyStoreError::TtlRejected`].
    async fn create(&self, k: NewApiKey<'_>) -> Result<ApiKey, ApiKeyStoreError>;

    /// Find a non-revoked, non-expired key by its hash.
    async fn find_live_by_hash(&self, hash: &[u8]) -> Result<Option<ApiKey>, ApiKeyStoreError>;

    /// A user's keys in a workspace, newest first: the live ones and those
    /// revoked or expired within the last [`LIST_RETENTION_DAYS`] days.
    async fn list_for_user(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
    ) -> Result<Vec<ApiKey>, ApiKeyStoreError>;

    /// Revoke one live key of `user_id` in `workspace_id`. `false` when no
    /// such key.
    async fn revoke(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
        key_id: Uuid,
    ) -> Result<bool, ApiKeyStoreError>;

    /// Bump `last_used_at = now()` at most once a minute; ignored if the row
    /// doesn't exist. Bulk revocation (password change, member removal)
    /// lives in DB triggers, so every writer is covered.
    async fn touch(&self, key_id: Uuid) -> Result<(), ApiKeyStoreError>;
}

#[derive(Clone)]
pub struct PgApiKeyStore {
    pool: PgPool,
}

impl PgApiKeyStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Column list shared by every read. Never includes `key_hash`.
const COLUMNS: &str = "id, workspace_id, user_id, name, scopes, created_by, created_at, \
                       expires_at, last_used_at, revoked_at";

type ApiKeyRow = (
    Uuid,
    Uuid,
    Uuid,
    String,
    Vec<String>,
    Uuid,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);

fn api_key_from_row(r: ApiKeyRow) -> Result<ApiKey, ApiKeyStoreError> {
    Ok(ApiKey {
        id: r.0,
        workspace_id: r.1,
        user_id: r.2,
        name: r.3,
        scopes: Scopes::from_names(&r.4)?,
        created_by: r.5,
        created_at: r.6,
        expires_at: r.7,
        last_used_at: r.8,
        revoked_at: r.9,
    })
}

fn map_insert_err(e: sqlx::Error) -> ApiKeyStoreError {
    if let sqlx::Error::Database(db) = &e
        && db.code().as_deref() == Some(SSO_TTL_ERRCODE)
    {
        return ApiKeyStoreError::TtlRejected;
    }
    ApiKeyStoreError::Sqlx(e)
}

#[async_trait]
impl ApiKeyStore for PgApiKeyStore {
    async fn create(&self, k: NewApiKey<'_>) -> Result<ApiKey, ApiKeyStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        // In id order, so two mints never wait on each other crosswise.
        let users: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT id, kind FROM users WHERE id = ANY($1) ORDER BY id FOR NO KEY UPDATE",
        )
        .bind([k.user_id, k.created_by].as_slice())
        .fetch_all(&mut *tx)
        .await?;
        if let Some(hash) = k.session_hash {
            let alive: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM sessions
                                 WHERE id = $1 AND user_id = $2 AND workspace_id = $3
                                   AND expires_at > now())",
            )
            .bind(hash)
            .bind(k.created_by)
            .bind(k.workspace_id)
            .fetch_one(&mut *tx)
            .await?;
            if !alive {
                return Err(ApiKeyStoreError::SessionEnded);
            }
        }
        let members: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT user_id, role FROM workspace_members
              WHERE workspace_id = $1 AND user_id = ANY($2)
              ORDER BY user_id FOR SHARE",
        )
        .bind(k.workspace_id)
        .bind([k.user_id, k.created_by].as_slice())
        .fetch_all(&mut *tx)
        .await?;
        let role_of = |id: Uuid| members.iter().find(|(u, _)| *u == id).map(|(_, r)| r);
        let Some(creator_role) = role_of(k.created_by) else {
            return Err(ApiKeyStoreError::SessionEnded);
        };
        if k.user_id != k.created_by {
            let target_is_bot = users
                .iter()
                .any(|(u, kind)| *u == k.user_id && kind == "bot");
            if creator_role != "owner" || !target_is_bot || role_of(k.user_id).is_none() {
                return Err(ApiKeyStoreError::NotPermitted);
            }
        }
        let live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM api_keys
              WHERE user_id = $1 AND revoked_at IS NULL
                AND (expires_at IS NULL OR expires_at > now())",
        )
        .bind(k.user_id)
        .fetch_one(&mut *tx)
        .await?;
        if live >= MAX_LIVE_KEYS_PER_USER {
            return Err(ApiKeyStoreError::LimitReached);
        }
        let row = sqlx::query_as::<_, ApiKeyRow>(sqlx::AssertSqlSafe(format!(
            "INSERT INTO api_keys
                 (workspace_id, user_id, name, key_hash, scopes, created_by, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING {COLUMNS}"
        )))
        .bind(k.workspace_id)
        .bind(k.user_id)
        .bind(k.name)
        .bind(k.key_hash)
        .bind(k.scopes.names())
        .bind(k.created_by)
        .bind(k.expires_at)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_insert_err)?;
        tx.commit().await?;
        api_key_from_row(row)
    }

    async fn find_live_by_hash(&self, hash: &[u8]) -> Result<Option<ApiKey>, ApiKeyStoreError> {
        let row = sqlx::query_as::<_, ApiKeyRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM api_keys
             WHERE key_hash = $1
               AND revoked_at IS NULL
               AND (expires_at IS NULL OR expires_at > now())"
        )))
        .bind(hash)
        .fetch_optional(&self.pool)
        .await?;
        row.map(api_key_from_row).transpose()
    }

    async fn list_for_user(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
    ) -> Result<Vec<ApiKey>, ApiKeyStoreError> {
        let rows = sqlx::query_as::<_, ApiKeyRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM api_keys
             WHERE workspace_id = $1 AND user_id = $2
               AND (revoked_at IS NULL OR revoked_at > now() - make_interval(days => $3))
               AND (expires_at IS NULL OR expires_at > now() - make_interval(days => $3))
             ORDER BY created_at DESC, id"
        )))
        .bind(workspace_id)
        .bind(user_id)
        .bind(LIST_RETENTION_DAYS)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(api_key_from_row).collect()
    }

    async fn revoke(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
        key_id: Uuid,
    ) -> Result<bool, ApiKeyStoreError> {
        let res = sqlx::query(
            "UPDATE api_keys SET revoked_at = now()
             WHERE id = $1 AND workspace_id = $2 AND user_id = $3 AND revoked_at IS NULL",
        )
        .bind(key_id)
        .bind(workspace_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    async fn touch(&self, key_id: Uuid) -> Result<(), ApiKeyStoreError> {
        // Debounced: busy agents would otherwise queue on this one row lock.
        sqlx::query(
            "UPDATE api_keys SET last_used_at = now()
             WHERE id = $1
               AND revoked_at IS NULL
               AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')",
        )
        .bind(key_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn scopes_parse_and_render_in_stable_order() {
        let s = Scopes::from_names(&names(&["delete", "read", "read"])).unwrap();
        assert!(s.contains(Scope::Read));
        assert!(s.contains(Scope::Delete));
        assert!(!s.contains(Scope::Create));
        assert!(!s.contains(Scope::Update));
        assert_eq!(s.names(), names(&["read", "delete"]));
    }

    #[test]
    fn scopes_reject_empty_and_unknown() {
        assert_eq!(Scopes::from_names(&[]), Err(ScopeError::Empty));
        assert_eq!(
            Scopes::from_names(&names(&["read", "admin"])),
            Err(ScopeError::Unknown("admin".into()))
        );
        assert_eq!(
            Scopes::from_names(&names(&[""])),
            Err(ScopeError::Unknown(String::new()))
        );
    }

    #[test]
    fn scope_parse_round_trips() {
        for s in Scope::ALL {
            assert_eq!(Scope::parse(s.as_str()), Some(s));
        }
        assert_eq!(Scope::parse("Read"), None);
    }
}
