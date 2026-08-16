//! Users CRUD — local credentials and OIDC linkage.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{AssertSqlSafe, PgPool};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub password_hash: Option<String>,
    pub oidc_subject: Option<String>,
    pub oidc_issuer: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum UserStoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("email already exists")]
    EmailExists,
    #[error("oidc subject already linked")]
    OidcExists,
}

#[async_trait]
pub trait UserStore: Send + Sync + 'static {
    async fn create_local(
        &self,
        email: &str,
        display_name: &str,
        password_hash: &str,
    ) -> Result<User, UserStoreError>;

    async fn create_oidc(
        &self,
        email: &str,
        display_name: &str,
        issuer: &str,
        subject: &str,
    ) -> Result<User, UserStoreError>;

    async fn find_by_email(&self, email: &str) -> Result<Option<User>, UserStoreError>;
    async fn find_by_oidc(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<User>, UserStoreError>;
    async fn find_by_id(&self, id: Uuid) -> Result<Option<User>, UserStoreError>;
    async fn count(&self) -> Result<i64, UserStoreError>;
}

#[derive(Clone)]
pub struct PgUserStore {
    pool: PgPool,
}

impl PgUserStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

type UserRow = (
    Uuid,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    DateTime<Utc>,
);

fn user_from_row(r: UserRow) -> User {
    User {
        id: r.0,
        email: r.1,
        display_name: r.2,
        password_hash: r.3,
        oidc_subject: r.4,
        oidc_issuer: r.5,
        created_at: r.6,
    }
}

const EMAIL_UNIQUE_CONSTRAINT: &str = "users_email_key";
const OIDC_UNIQUE_CONSTRAINT: &str = "users_oidc_issuer_oidc_subject_key";

/// Map a sqlx error to the right `UserStoreError` variant based on which
/// unique constraint was violated. Falls through to `Sqlx` for non-unique
/// errors or any unrecognised constraint name.
fn map_user_violation(e: sqlx::Error) -> UserStoreError {
    if let sqlx::Error::Database(ref db) = e
        && db.is_unique_violation()
    {
        match db.constraint() {
            Some(EMAIL_UNIQUE_CONSTRAINT) => return UserStoreError::EmailExists,
            Some(OIDC_UNIQUE_CONSTRAINT) => return UserStoreError::OidcExists,
            _ => {}
        }
    }
    UserStoreError::Sqlx(e)
}

/// Shared SELECT column list. As with `doc_store::COLS`, this constant is the
/// only value interpolated into the query strings that `AssertSqlSafe` wraps;
/// all caller-supplied values stay `$N` binds.
const SELECT_USER_COLS: &str =
    "id, email::text, display_name, password_hash, oidc_subject, oidc_issuer, created_at";

#[async_trait]
impl UserStore for PgUserStore {
    async fn create_local(
        &self,
        email: &str,
        display_name: &str,
        password_hash: &str,
    ) -> Result<User, UserStoreError> {
        let row = sqlx::query_as::<_, UserRow>(AssertSqlSafe(format!(
            "INSERT INTO users (email, display_name, password_hash)
             VALUES ($1, $2, $3)
             RETURNING {SELECT_USER_COLS}"
        )))
        .bind(email)
        .bind(display_name)
        .bind(password_hash)
        .fetch_one(&self.pool)
        .await
        .map_err(map_user_violation)?;
        Ok(user_from_row(row))
    }

    async fn create_oidc(
        &self,
        email: &str,
        display_name: &str,
        issuer: &str,
        subject: &str,
    ) -> Result<User, UserStoreError> {
        let row = sqlx::query_as::<_, UserRow>(AssertSqlSafe(format!(
            "INSERT INTO users (email, display_name, oidc_issuer, oidc_subject)
             VALUES ($1, $2, $3, $4)
             RETURNING {SELECT_USER_COLS}"
        )))
        .bind(email)
        .bind(display_name)
        .bind(issuer)
        .bind(subject)
        .fetch_one(&self.pool)
        .await
        .map_err(map_user_violation)?;
        Ok(user_from_row(row))
    }

    async fn find_by_email(&self, email: &str) -> Result<Option<User>, UserStoreError> {
        // Cast the bound parameter to citext so the comparison uses
        // citext (case-insensitive) semantics; binding a Rust &str produces
        // a plain `text` parameter which would do a case-sensitive compare.
        let row = sqlx::query_as::<_, UserRow>(AssertSqlSafe(format!(
            "SELECT {SELECT_USER_COLS} FROM users WHERE email = $1::citext"
        )))
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(user_from_row))
    }

    async fn find_by_oidc(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<User>, UserStoreError> {
        let row = sqlx::query_as::<_, UserRow>(AssertSqlSafe(format!(
            "SELECT {SELECT_USER_COLS} FROM users
             WHERE oidc_issuer = $1 AND oidc_subject = $2"
        )))
        .bind(issuer)
        .bind(subject)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(user_from_row))
    }

    async fn find_by_id(&self, id: Uuid) -> Result<Option<User>, UserStoreError> {
        let row = sqlx::query_as::<_, UserRow>(AssertSqlSafe(format!(
            "SELECT {SELECT_USER_COLS} FROM users WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(user_from_row))
    }

    async fn count(&self) -> Result<i64, UserStoreError> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await?;
        Ok(n)
    }
}
