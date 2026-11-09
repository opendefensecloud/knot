//! Short page links: a 7-character code that names a page. A code grants
//! nothing; whoever opens it still needs read access to the page.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rand::TryRng;
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum ShortLinkStoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

pub type Result<T> = std::result::Result<T, ShortLinkStoreError>;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ShortLink {
    pub code: String,
    pub doc_id: Uuid,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[async_trait]
pub trait ShortLinkStore: Send + Sync {
    /// The page's code, creating one on first use. Concurrent first calls
    /// for one page all get the same code.
    async fn get_or_create(&self, doc_id: Uuid, created_by: Uuid) -> Result<ShortLink>;
    /// The page's code, if it has one. Never creates.
    async fn find_for_doc(&self, doc_id: Uuid) -> Result<Option<ShortLink>>;
    /// The page a code names. No access check: callers check read access.
    async fn resolve(&self, code: &str) -> Result<Option<ShortLink>>;
}

pub struct PgShortLinkStore {
    pool: PgPool,
}

impl PgShortLinkStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Fresh codes tried before giving up; a collision at 56^7 codes is rare
/// enough that more than one retry means something else is wrong.
const CREATE_ATTEMPTS: usize = 5;

#[async_trait]
impl ShortLinkStore for PgShortLinkStore {
    async fn get_or_create(&self, doc_id: Uuid, created_by: Uuid) -> Result<ShortLink> {
        for _ in 0..CREATE_ATTEMPTS {
            if let Some(existing) = self.find_for_doc(doc_id).await? {
                return Ok(existing);
            }
            // ON CONFLICT covers both unique columns: another caller won the
            // page (found on the next pass), or the code collided (redrawn).
            sqlx::query(
                "INSERT INTO doc_short_links (code, doc_id, created_by) VALUES ($1, $2, $3) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(new_short_code())
            .bind(doc_id)
            .bind(created_by)
            .execute(&self.pool)
            .await?;
        }
        if let Some(existing) = self.find_for_doc(doc_id).await? {
            return Ok(existing);
        }
        // Still nothing: insert without ON CONFLICT so the real error
        // (e.g. a purged page's FK violation) surfaces instead of a silent miss.
        let row = sqlx::query_as::<_, ShortLink>(
            "INSERT INTO doc_short_links (code, doc_id, created_by) VALUES ($1, $2, $3) \
             RETURNING code, doc_id, created_by, created_at",
        )
        .bind(new_short_code())
        .bind(doc_id)
        .bind(created_by)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn find_for_doc(&self, doc_id: Uuid) -> Result<Option<ShortLink>> {
        Ok(sqlx::query_as::<_, ShortLink>(
            "SELECT code, doc_id, created_by, created_at FROM doc_short_links WHERE doc_id = $1",
        )
        .bind(doc_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn resolve(&self, code: &str) -> Result<Option<ShortLink>> {
        Ok(sqlx::query_as::<_, ShortLink>(
            "SELECT code, doc_id, created_by, created_at FROM doc_short_links WHERE code = $1",
        )
        .bind(code)
        .fetch_optional(&self.pool)
        .await?)
    }
}

/// Length of every short code.
pub const SHORT_CODE_LEN: usize = 7;

/// No look-alikes (`0 O 1 l I o`). Digits first: [`is_short_code`] relies
/// on every code holding one, which keeps codes apart from app routes
/// (`/login`, `/tasks`, …), none of which contains a digit.
const ALPHABET: &[u8] = b"23456789abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ";

/// A fresh random code: [`SHORT_CODE_LEN`] symbols of [`ALPHABET`], at
/// least one of them a digit.
pub fn new_short_code() -> String {
    // Rejection sampling: 256 is not a multiple of 56, so only bytes below
    // 224 = 4 × 56 are used, which keeps every symbol equally likely.
    const LIMIT: u8 = (256 / ALPHABET.len() * ALPHABET.len()) as u8;
    loop {
        let mut code = String::with_capacity(SHORT_CODE_LEN);
        while code.len() < SHORT_CODE_LEN {
            let mut b = [0u8; 16];
            // As share_tokens.rs: a failing OS CSPRNG must not degrade into
            // predictable codes.
            rand::rngs::SysRng
                .try_fill_bytes(&mut b)
                .expect("OS CSPRNG unavailable");
            for byte in b.into_iter().filter(|&x| x < LIMIT) {
                if code.len() == SHORT_CODE_LEN {
                    break;
                }
                code.push(ALPHABET[byte as usize % ALPHABET.len()] as char);
            }
        }
        if is_short_code(&code) {
            return code;
        }
    }
}

/// Whether `s` has the shape of a short code: [`SHORT_CODE_LEN`] symbols
/// of [`ALPHABET`] including a digit. Says nothing about whether it exists.
pub fn is_short_code(s: &str) -> bool {
    s.len() == SHORT_CODE_LEN
        && s.bytes().all(|b| ALPHABET.contains(&b))
        && s.bytes().any(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::{SHORT_CODE_LEN, is_short_code, new_short_code};

    #[test]
    fn generated_codes_are_valid() {
        for _ in 0..10_000 {
            let code = new_short_code();
            assert_eq!(code.len(), SHORT_CODE_LEN);
            assert!(
                code.chars().all(
                    |c| "23456789abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ".contains(c)
                ),
                "{code}"
            );
            assert!(
                code.chars().any(|c| c.is_ascii_digit()),
                "{code} has no digit"
            );
            assert!(is_short_code(&code), "{code}");
        }
    }

    #[test]
    fn is_short_code_rejects() {
        for s in [
            "abcdefg", "k3x9Fa", "k3x9Fa77", "k3x9Fl7", "k3x0Fa7", "k3x9Fa/", "",
        ] {
            assert!(!is_short_code(s), "{s:?} must be rejected");
        }
    }

    #[test]
    fn generated_codes_never_match_an_app_route() {
        for name in [
            "login",
            "setup",
            "tasks",
            "members",
            "settings",
            "templates",
            "notifications",
            "archive",
            "about",
            "library-return",
            "api",
            "auth",
            "collab",
            "p",
            "doc",
            "assets",
            "favicon.ico",
        ] {
            assert!(!is_short_code(name), "{name} must never be a code");
        }
    }
}
