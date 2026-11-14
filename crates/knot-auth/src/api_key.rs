//! Scoped API keys: generation, format check, at-rest hash, TTL policy.
//!
//! A key is `knot_` + base64url-no-pad of 32 CSPRNG bytes (48 chars total).
//! Only `HMAC-SHA256(session_key, secret)` is stored, deterministic so a
//! unique-index lookup suffices. Consequence: rotating `KNOT_SESSION_KEY`
//! invalidates every session AND every API key.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::TryRng;
use thiserror::Error;

pub const KEY_PREFIX: &str = "knot_";
pub const SECRET_BYTES: usize = 32;
pub const MIN_TTL_DAYS: i64 = 30;
pub const SSO_MAX_TTL_DAYS: i64 = 90;

/// base64url-no-pad length of `SECRET_BYTES` bytes.
const ENCODED_LEN: usize = (SECRET_BYTES * 4).div_ceil(3);

/// Plaintext API key. Exists exactly once (the 201 response); `Debug`
/// redacts it.
#[derive(Clone, Eq, PartialEq)]
pub struct ApiKeySecret(String);

impl ApiKeySecret {
    pub fn generate() -> Self {
        let mut buf = [0u8; SECRET_BYTES];
        // Same rationale as SessionToken::generate: panic rather than mint a
        // predictable key if the OS CSPRNG fails.
        rand::rngs::SysRng
            .try_fill_bytes(&mut buf)
            .expect("OS CSPRNG unavailable");
        Self(format!("{KEY_PREFIX}{}", URL_SAFE_NO_PAD.encode(buf)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiKeySecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKeySecret(***)")
    }
}

/// Cheap syntactic check (prefix + exact base64url body), no allocation.
pub fn looks_like_api_key(s: &str) -> bool {
    let Some(body) = s.strip_prefix(KEY_PREFIX) else {
        return false;
    };
    body.len() == ENCODED_LEN
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// At-rest form of a key: HMAC-SHA256 keyed by the server session key.
pub fn hash(secret: &str, session_key: &[u8]) -> Vec<u8> {
    crate::csrf::hash_session_id(session_key, secret.as_bytes())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrincipalKind {
    /// Human with a local password.
    Local,
    /// Human with only an OIDC identity (no password to rotate).
    Oidc,
    /// Credential-less bot user.
    Bot,
}

pub fn max_ttl_days(kind: PrincipalKind) -> Option<i64> {
    match kind {
        PrincipalKind::Oidc => Some(SSO_MAX_TTL_DAYS),
        PrincipalKind::Local | PrincipalKind::Bot => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TtlError {
    #[error("api key ttl too short")]
    TooShort,
    #[error("api key ttl too long")]
    TooLong,
}

/// `None` = never expires. Minimum is checked before the kind cap; pure-OIDC
/// users may not have unlimited keys (they cannot revoke via password change).
pub fn validate_ttl_days(kind: PrincipalKind, days: Option<i64>) -> Result<Option<i64>, TtlError> {
    if let Some(d) = days
        && d < MIN_TTL_DAYS
    {
        return Err(TtlError::TooShort);
    }
    match (max_ttl_days(kind), days) {
        (None, d) => Ok(d),
        (Some(_), None) => Err(TtlError::TooLong),
        (Some(max), Some(d)) if d > max => Err(TtlError::TooLong),
        (Some(_), Some(d)) => Ok(Some(d)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionToken;

    #[test]
    fn generated_key_has_prefix_and_exact_length() {
        let k = ApiKeySecret::generate();
        assert!(k.as_str().starts_with(KEY_PREFIX));
        assert_eq!(k.as_str().len(), KEY_PREFIX.len() + 43);
        assert!(!k.as_str().contains('='));
        assert!(looks_like_api_key(k.as_str()));
    }

    #[test]
    fn generated_key_decodes_to_32_bytes() {
        let k = ApiKeySecret::generate();
        let body = k.as_str().strip_prefix(KEY_PREFIX).unwrap();
        let bytes = URL_SAFE_NO_PAD.decode(body).expect("decode");
        assert_eq!(bytes.len(), SECRET_BYTES);
        assert_eq!(URL_SAFE_NO_PAD.encode(&bytes), body);
    }

    #[test]
    fn two_keys_are_unique() {
        assert_ne!(ApiKeySecret::generate(), ApiKeySecret::generate());
    }

    #[test]
    fn debug_redacts_secret() {
        let k = ApiKeySecret::generate();
        let s = format!("{k:?}");
        assert_eq!(s, "ApiKeySecret(***)");
        assert!(!s.contains(k.as_str()));
    }

    #[test]
    fn looks_like_api_key_rejects_malformed() {
        let k = ApiKeySecret::generate();
        let s = k.as_str();
        assert!(!looks_like_api_key(""));
        assert!(!looks_like_api_key(KEY_PREFIX));
        assert!(!looks_like_api_key(&s[..s.len() - 1]));
        assert!(!looks_like_api_key(&format!("{s}A")));
        assert!(!looks_like_api_key(&format!("{}=", &s[..s.len() - 1])));
        assert!(!looks_like_api_key(&format!("{}+", &s[..s.len() - 1])));
        assert!(!looks_like_api_key(&format!(
            "KNOT_{}",
            &s[KEY_PREFIX.len()..]
        )));
    }

    #[test]
    fn looks_like_api_key_rejects_bare_session_token() {
        let t = SessionToken::generate().encode();
        assert!(!looks_like_api_key(&t));
    }

    #[test]
    fn hash_is_deterministic_and_depends_on_session_key() {
        let k1 = b"test-key-32-bytes-aaaaaaaaaaaaaa";
        let k2 = b"other-key-32-bytes-bbbbbbbbbbbbbb";
        let s = ApiKeySecret::generate();
        let h1 = hash(s.as_str(), k1);
        assert_eq!(h1, hash(s.as_str(), k1));
        assert_eq!(h1.len(), 32);
        assert_ne!(h1, hash(s.as_str(), k2));
        assert_ne!(h1, hash(ApiKeySecret::generate().as_str(), k1));
    }

    #[test]
    fn max_ttl_only_caps_oidc() {
        assert_eq!(max_ttl_days(PrincipalKind::Oidc), Some(90));
        assert_eq!(max_ttl_days(PrincipalKind::Local), None);
        assert_eq!(max_ttl_days(PrincipalKind::Bot), None);
    }

    #[test]
    fn ttl_matrix() {
        use PrincipalKind::*;
        use TtlError::*;
        assert_eq!(validate_ttl_days(Bot, None), Ok(None));
        assert_eq!(validate_ttl_days(Local, None), Ok(None));
        assert_eq!(validate_ttl_days(Oidc, None), Err(TooLong));
        assert_eq!(validate_ttl_days(Oidc, Some(91)), Err(TooLong));
        assert_eq!(validate_ttl_days(Oidc, Some(90)), Ok(Some(90)));
        assert_eq!(validate_ttl_days(Oidc, Some(10)), Err(TooShort));
        assert_eq!(validate_ttl_days(Local, Some(10)), Err(TooShort));
        assert_eq!(validate_ttl_days(Local, Some(29)), Err(TooShort));
        assert_eq!(validate_ttl_days(Local, Some(30)), Ok(Some(30)));
        assert_eq!(validate_ttl_days(Bot, Some(0)), Err(TooShort));
        assert_eq!(validate_ttl_days(Bot, Some(3650)), Ok(Some(3650)));
    }
}
