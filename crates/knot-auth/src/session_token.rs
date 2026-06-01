//! 32-byte cryptographic session token + base64url codec.
//!
//! Tokens are produced by the OS CSPRNG. Storage uses the raw 32 bytes
//! (as `bytea` in the `sessions` table); the cookie carries the
//! base64url-encoded form (43 chars, no padding).

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use thiserror::Error;

pub const TOKEN_BYTES: usize = 32;

#[derive(Debug, Error)]
pub enum TokenError {
    #[error("malformed token")]
    Malformed,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SessionToken([u8; TOKEN_BYTES]);

impl SessionToken {
    pub fn generate() -> Self {
        let mut buf = [0u8; TOKEN_BYTES];
        rand::rngs::OsRng.fill_bytes(&mut buf);
        Self(buf)
    }

    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    pub fn decode(s: &str) -> Result<Self, TokenError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(s)
            .map_err(|_| TokenError::Malformed)?;
        if bytes.len() != TOKEN_BYTES {
            return Err(TokenError::Malformed);
        }
        let mut buf = [0u8; TOKEN_BYTES];
        buf.copy_from_slice(&bytes);
        Ok(Self(buf))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

// `Debug` does NOT print bytes — these are credentials.
impl std::fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionToken(***)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_returns_32_bytes() {
        let t = SessionToken::generate();
        assert_eq!(t.as_bytes().len(), 32);
    }

    #[test]
    fn encode_decode_roundtrip() {
        let t = SessionToken::generate();
        let s = t.encode();
        let d = SessionToken::decode(&s).expect("decode");
        assert_eq!(t, d);
    }

    #[test]
    fn encoded_token_is_43_chars_no_padding() {
        let t = SessionToken::generate();
        let s = t.encode();
        assert_eq!(s.len(), 43);
        assert!(!s.contains('='));
    }

    #[test]
    fn decode_rejects_wrong_length() {
        let err = SessionToken::decode("too-short").unwrap_err();
        assert!(matches!(err, TokenError::Malformed));
    }

    #[test]
    fn two_tokens_are_unique() {
        let a = SessionToken::generate();
        let b = SessionToken::generate();
        assert_ne!(a, b);
    }

    #[test]
    fn debug_redacts_bytes() {
        let t = SessionToken::generate();
        let s = format!("{:?}", t);
        assert_eq!(s, "SessionToken(***)");
    }
}
