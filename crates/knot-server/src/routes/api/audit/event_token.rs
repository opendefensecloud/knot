//! The opaque ids the page audit log sends for its events, and takes back
//! as `cursor`.
//!
//! A stored event id is never sent. One export writes its rows (one per
//! page in the file) in one statement, so their ids are consecutive: a
//! reader who saw the raw ids on the pages they can open would count, from
//! the gaps, the pages of the export they can't open, which is what
//! `doc_count` is withheld for.
//!
//! The token is the id run through a keyed permutation of 64-bit numbers:
//! an eight-round Feistel network whose round function is HMAC-SHA256,
//! keyed by a key derived from the server's `session_key` and tweaked by
//! the page id, written as 16 hex digits. Every replica with the same key
//! reads every replica's cursors; two pages' tokens for nearby ids look
//! unrelated, and a token means nothing on another page.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// Labels the key derived from `session_key`, so these tokens share
/// nothing with the session and CSRF hashes made from the same key.
const KEY_LABEL: &[u8] = b"knot.audit.event-token.v1";

const ROUNDS: u8 = 8;

/// The digits of a token: a `u64` in hex.
const TOKEN_LENGTH: usize = 16;

/// Seals and opens the event ids of one page's audit log.
#[derive(Clone)]
pub struct EventTokens {
    /// Keyed by the derived key, with the page id already fed in.
    page_mac: HmacSha256,
}

impl EventTokens {
    /// The tokens of `doc_id`'s log under `session_key`.
    pub fn new(session_key: &[u8], doc_id: Uuid) -> Self {
        let mut key_mac = HmacSha256::new_from_slice(session_key).expect("hmac takes any key");
        key_mac.update(KEY_LABEL);
        let derived_key = key_mac.finalize().into_bytes();
        let mut page_mac = HmacSha256::new_from_slice(&derived_key).expect("hmac takes any key");
        page_mac.update(doc_id.as_bytes());
        Self { page_mac }
    }

    fn round(&self, round: u8, half: u32) -> u32 {
        let mut mac = self.page_mac.clone();
        mac.update(&[round]);
        mac.update(&half.to_be_bytes());
        let digest = mac.finalize().into_bytes();
        u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
    }

    /// The token `id` is shown as.
    pub fn seal(&self, id: i64) -> String {
        let plain = id as u64;
        let (mut left, mut right) = ((plain >> 32) as u32, plain as u32);
        for round in 0..ROUNDS {
            (left, right) = (right, left ^ self.round(round, right));
        }
        format!("{:016x}", (u64::from(left) << 32) | u64::from(right))
    }

    /// The positive event id `token` stands for, or `None` for text that is
    /// no token.
    pub fn open(&self, token: &str) -> Option<i64> {
        if token.len() != TOKEN_LENGTH || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let sealed = u64::from_str_radix(token, 16).ok()?;
        let (mut left, mut right) = ((sealed >> 32) as u32, sealed as u32);
        for round in (0..ROUNDS).rev() {
            (left, right) = (right ^ self.round(round, left), left);
        }
        let id = i64::try_from((u64::from(left) << 32) | u64::from(right)).ok()?;
        (id > 0).then_some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"test-key-32-bytes-aaaaaaaaaaaaaa";

    #[test]
    fn a_token_opens_to_its_id() {
        let tokens = EventTokens::new(KEY, Uuid::new_v4());
        for id in [1, 2, 3, 12, 1_000, 1_001, 4_294_967_296, i64::MAX] {
            let token = tokens.seal(id);
            assert_eq!(token.len(), TOKEN_LENGTH, "{token}");
            assert_eq!(tokens.open(&token), Some(id), "{id} as {token}");
            assert_eq!(tokens.open(&token.to_uppercase()), Some(id));
        }
    }

    #[test]
    fn consecutive_ids_give_unrelated_tokens() {
        let tokens = EventTokens::new(KEY, Uuid::new_v4());
        let sealed: Vec<u64> = (1000..1004)
            .map(|id| u64::from_str_radix(&tokens.seal(id), 16).unwrap())
            .collect();
        for (id, token) in (1000..1004).zip(&sealed) {
            assert_ne!(*token, id as u64);
        }
        let mut distinct = sealed.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), sealed.len(), "{sealed:x?}");
        // Nearby ids don't seal to nearby tokens.
        for pair in sealed.windows(2) {
            assert!(pair[0].abs_diff(pair[1]) > 1 << 16, "{sealed:x?}");
        }
    }

    #[test]
    fn tokens_depend_on_the_page_and_the_key() {
        let page = Uuid::new_v4();
        let tokens = EventTokens::new(KEY, page);
        assert_eq!(EventTokens::new(KEY, page).seal(42), tokens.seal(42));
        assert_ne!(
            EventTokens::new(KEY, Uuid::new_v4()).seal(42),
            tokens.seal(42)
        );
        assert_ne!(
            EventTokens::new(b"other key", page).seal(42),
            tokens.seal(42)
        );
    }

    #[test]
    fn text_that_is_no_token_is_refused() {
        let tokens = EventTokens::new(KEY, Uuid::new_v4());
        for text in [
            "",
            "12",
            "abc",
            "0",
            "+00000000000000a",
            "zzzzzzzzzzzzzzzz",
            "00000000000000000",
            "éééééééé",
        ] {
            assert_eq!(tokens.open(text), None, "{text:?}");
        }
        // Every 64-bit value opens to some number: zero and the negative
        // ones are no event id.
        for not_an_id in [0, -1, i64::MIN] {
            assert_eq!(tokens.open(&tokens.seal(not_an_id)), None, "{not_an_id}");
        }
    }
}
