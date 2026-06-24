//! Double-submit CSRF tokens.
//!
//! The server mints a token bound to the session id via HMAC-SHA256 keyed
//! on the configured `session_key`. The client echoes it in an
//! `X-CSRF-Token` header on unsafe-method requests; the server verifies
//! the HMAC matches the cookie. Tokens are NOT sent back to storage — the
//! HMAC IS the validation.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Derive the at-rest session id from the raw cookie token, keyed by the
/// server secret. Storing this (not the raw token) means a leaked `sessions`
/// table is useless without the externally-held key, and rotating the key
/// invalidates every session.
pub fn hash_session_id(key: &[u8], token: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
    mac.update(token);
    mac.finalize().into_bytes().to_vec()
}

pub fn mint(key: &[u8], session_id: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
    mac.update(session_id);
    let tag = mac.finalize().into_bytes();
    URL_SAFE_NO_PAD.encode(tag)
}

pub fn verify(key: &[u8], session_id: &[u8], token: &str) -> bool {
    let Ok(provided) = URL_SAFE_NO_PAD.decode(token) else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
    mac.update(session_id);
    mac.verify_slice(&provided).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"test-key-32-bytes-aaaaaaaaaaaaaa";

    #[test]
    fn hash_session_id_is_deterministic_and_32_bytes() {
        let k = b"test-key-32-bytes-aaaaaaaaaaaaaa";
        let t = b"raw-token";
        let h1 = hash_session_id(k, t);
        let h2 = hash_session_id(k, t);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 32);
    }

    #[test]
    fn hash_session_id_differs_for_different_key() {
        let k1 = b"test-key-32-bytes-aaaaaaaaaaaaaa";
        let k2 = b"other-key-32-bytes-bbbbbbbbbbbbbb";
        let t = b"raw-token";
        assert_ne!(hash_session_id(k1, t), hash_session_id(k2, t));
    }

    #[test]
    fn roundtrip() {
        let sid = b"sid";
        let t = mint(KEY, sid);
        assert!(verify(KEY, sid, &t));
    }

    #[test]
    fn rejects_different_session() {
        let t = mint(KEY, b"sid-1");
        assert!(!verify(KEY, b"sid-2", &t));
    }

    #[test]
    fn rejects_corrupt_token() {
        assert!(!verify(KEY, b"sid", "not-base64!"));
        assert!(!verify(KEY, b"sid", "AAA"));
    }

    #[test]
    fn rejects_different_key() {
        let t = mint(KEY, b"sid");
        let other_key: &[u8] = b"other-key-32-bytes-bbbbbbbbbbbbb";
        assert!(!verify(other_key, b"sid", &t));
    }
}
